const chokidar = require('chokidar');
const connectLivereload = require("connect-livereload");
const express = require('express');
const fs = require('fs');
const livereload = require("livereload");
const path = require('path');
const { spawn } = require("child_process");

const app = express();

app.use(connectLivereload());

app.set('view engine', 'ejs');
app.use(express.static(path.join(__dirname, "public")));

let dots = [];
app.get('', (req, res) => {
    res.render('index', {
        dots,
    });
});

const dotDir = process.env.GST_DEBUG_DUMP_DOT_DIR;
const svgDir = './public/svg/';

if (fs.existsSync(svgDir)) {
    fs.rmSync(svgDir, { recursive: true });
}
fs.mkdirSync(svgDir);
chokidar.watch((dotDir ? dotDir : '.') + '/*dot').on('all', async (event, dotFile) => {
    const svg_file = svgDir + path.basename(dotFile).replace('.dot', '.svg');
    const html_file = svgDir + path.basename(dotFile).replace('.dot', '.html');

    if (event == 'add') {
        spawn('dot', ['-Tsvg', dotFile, '-o', svg_file]);

        fs.readFile('templates/single_graph_template.html', 'utf8', (err, template) => {
            if (err) {
                console.error(err);
                return;
            }

            // Generate an HTML file from the Template
            const graphHtml = template.replace('{{ svg_file_path }}', `${path.basename(dotFile).replace('.dot', '.svg')}`);

            // Save the generated HTML to a file
            fs.writeFile(`${html_file}`, graphHtml, (err) => {
                if (err) {
                    console.error(err);
                } else {
                    console.log(`${html_file} has been saved!`);
                }
            });
        });
    } else if (event == 'unlink') {
        if (fs.existsSync(svg_file)) {
            fs.rmSync(svg_file);
        }
        if (fs.existsSync(html_file)) {
            fs.rmSync(html_file);
        }
    }
});

chokidar.watch('public/svg/*html').on('all', async (event, path) => {
    const html_file = path.replace('public/', '');
    const index = dots.indexOf(html_file);
    if (event == 'add' && index < 0) {
        dots.push(html_file);
    } else if (event == 'unlink' && index >= 0) {
        dots.splice(index, 1);
    }
    liveReloadServer.refresh("/");
});

app.listen(3000, () => {
    console.log('Watching logs in ' + (dotDir ? dotDir : '.'));
    console.log('Browse to http://localhost:3000');
});

const liveReloadServer = livereload.createServer({debug: false});
liveReloadServer.watch(path.join(__dirname, 'public', 'svg'));
